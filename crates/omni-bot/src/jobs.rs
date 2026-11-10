//! Generation of assistant turns: waiting for parents, attempts with retries,
//! streaming into a [`ReplyView`], persistence, usage accounting, and recovery
//! of generations interrupted by a restart.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use omni_core::content::{Part, Role};
use omni_core::{Turn, Usage};
use omni_llm::{Accumulator, CallInput, EventSink, LlmError, StreamEvent};
use rusqlite::params;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, error, info, info_span, warn};

use crate::app::App;
use crate::compose::Status;
use crate::conv::{self, AttemptUpdate, TurnRow, TurnStatus};
use crate::rawlog::{Blobs, RawLog};
use crate::store::now_ms;
use crate::tg::outbox::{self, Link};
use crate::view::{RenderState, ReplyView};

/// Start generating `turn_id` in the background (no-op if already running).
pub fn spawn_turn(app: Arc<App>, turn_id: i64) {
    if !app.running.lock().insert(turn_id) {
        return;
    }
    tokio::spawn(async move {
        let span = info_span!("turn", turn = turn_id);
        if let Err(e) = run_turn(app.clone(), turn_id).instrument(span).await {
            error!(turn = turn_id, error = %format!("{e:#}"), "turn failed internally");
            let _ = conv::set_turn_status(&app.db, turn_id, TurnStatus::Failed).await;
        }
        app.running.lock().remove(&turn_id);
        let _ = app.turn_done.send(turn_id);
    });
}

/// Resume everything that was in flight when the bot stopped.
pub async fn recover(app: Arc<App>) -> Result<()> {
    let turns = conv::unfinished_turns(&app.db).await?;
    if !turns.is_empty() {
        info!(count = turns.len(), "resuming unfinished generations");
    }
    for t in turns {
        spawn_turn(app.clone(), t);
    }
    Ok(())
}

fn show_usage(app: &App, usage: Option<&Usage>) -> bool {
    match app.cfg.render.show_usage.as_str() {
        "always" => true,
        "never" => false,
        t => match (t.parse::<f64>(), usage.and_then(|u| u.cost_usd)) {
            (Ok(th), Some(c)) => c >= th,
            _ => true,
        },
    }
}

async fn wait_for_ancestors(app: &App, turn_id: i64) -> Result<Result<Vec<TurnRow>, String>> {
    let mut rx = app.turn_done.subscribe();
    loop {
        let hist = conv::history(&app.db, turn_id).await?;
        let mut blocking = None;
        for t in &hist[..hist.len().saturating_sub(1)] {
            if t.turn.role == Role::Assistant {
                match t.status {
                    TurnStatus::Failed => {
                        return Ok(Err("the message this conversation continues from failed to generate".into()));
                    }
                    TurnStatus::Pending | TurnStatus::Running => blocking = Some(t.id),
                    TurnStatus::Done => {}
                }
            }
        }
        if blocking.is_none() {
            return Ok(Ok(hist));
        }
        // Wait for any turn to finish (or re-check periodically).
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match rx.recv().await {
                    Ok(id) if Some(id) == blocking => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        })
        .await;
    }
}

/// The user message a reply view should reply to.
async fn trigger_message(app: &App, row: &TurnRow) -> Option<i32> {
    let inbound = row.inbound_id?;
    conv::get_inbound(&app.db, inbound).await.ok().map(|i| i.msg_id)
}

async fn run_turn(app: Arc<App>, turn_id: i64) -> Result<()> {
    let row = conv::get_turn(&app.db, turn_id).await?;
    if row.status.is_finished() {
        return Ok(());
    }
    let spec = row.spec.clone().context("assistant turn without spec")?;
    let bot_name = row.bot.clone().context("turn without bot")?;
    let bot = app.bot(&bot_name).cloned().ok_or_else(|| anyhow!("bot {bot_name} is not configured any more"))?;
    let chat_id = row.chat_id.context("turn without chat")?;
    let user_id = row.user_id.unwrap_or(0);
    let span = info_span!("conv", bot = %bot_name, chat = chat_id, user = user_id, model = %spec.model);
    async move {
        let reply_to = trigger_message(&app, &row).await;
        let max_attempts = app.cfg.retry.max_attempts.max(1);
        let base_state = RenderState {
            label: spec.label.clone(),
            model: spec.model.clone(),
            options: spec.options_syntax(),
            parts: Vec::new(),
            status: Status::Generating,
            attempt: 1,
            max_attempts,
            error: None,
            usage: None,
            provider: None,
            finish_reason: None,
            native_finish_reason: None,
            notes: Vec::new(),
            started: Instant::now(),
            elapsed_override: None,
            images: 0,
            show_usage: true,
            mode: app.cfg.render.markdown,
            done: false,
        };

        let hist = match wait_for_ancestors(&app, turn_id).await? {
            Ok(h) => h,
            Err(msg) => {
                let err = LlmError::request(msg);
                fail_without_attempt(&app, &bot, chat_id, reply_to, turn_id, base_state, err).await?;
                return Ok(());
            }
        };

        // Interrupted attempt from a previous run of the bot.
        let attempts = conv::attempts_of(&app.db, turn_id).await?;
        if let Some(last) = attempts.last().filter(|a| a.status == "running") {
            warn!(attempt = last.n, "attempt was interrupted by a restart");
            let (_, out, _, _, _) = conv::attempt_json(&app.db, last.id).await?;
            let parts: Vec<Part> = out
                .and_then(|o| serde_json::from_value::<Turn>(o).ok())
                .map(|t| t.parts)
                .unwrap_or_default();
            conv::update_attempt(&app.db, last.id, AttemptUpdate {
                status: "interrupted",
                output: None,
                error: Some(json!({"message": "interrupted by bot restart"})),
                upstream_id: None,
                usage: None,
                finished: true,
            })
            .await?;
            let view = ReplyView::start(app.clone(), bot.clone(), chat_id, reply_to, turn_id, last.id, Duration::ZERO);
            let mut st = base_state.clone();
            st.parts = parts;
            st.status = Status::Interrupted;
            st.attempt = last.n;
            st.elapsed_override = Some(Duration::ZERO);
            view.finish(st).await;
        }
        let mut failed = attempts.iter().filter(|a| a.status == "failed").count() as u32;

        let history: Vec<Turn> = hist[..hist.len() - 1].iter().map(|t| t.turn.clone()).collect();
        let root = hist.first().map(|t| t.id).unwrap_or(turn_id);
        loop {
            if let Err(msg) = crate::access::check_quota(&app, chat_id, user_id).await? {
                fail_without_attempt(&app, &bot, chat_id, reply_to, turn_id, base_state.clone(), LlmError::request(msg)).await?;
                return Ok(());
            }
            let presets = app.presets_for(&spec.canonical());
            let input = CallInput {
                model: &spec.model,
                options: &spec.options,
                history: &history,
                session_id: Some(format!("tg-{root}")),
                user: app.user_hash(user_id),
            };
            let blobs = Blobs(app.blobs.clone());
            let prepared = match app.openrouter.prepare_chat(&input, &presets, &blobs).await {
                Ok(p) => p,
                Err(e) => {
                    fail_without_attempt(&app, &bot, chat_id, reply_to, turn_id, base_state.clone(), e).await?;
                    return Ok(());
                }
            };
            let rawlog = RawLog { blobs: app.blobs.clone() };
            let redacted_req = omni_core::json::redact_blobs(&prepared.body, &mut |b, m| {
                app.blobs.put_sync(b, m).unwrap_or_else(|_| omni_core::BlobRef::for_bytes(b, m))
            });
            let attempt = conv::start_attempt(&app.db, turn_id, &json!({
                "dialect": prepared.dialect,
                "url": prepared.url,
                "body": redacted_req,
                "applied": prepared.applied.iter().map(|a| json!({"source": a.source, "patch": a.patch})).collect::<Vec<_>>(),
            }))
            .await?;
            let attempt_span = info_span!("attempt", n = attempt.n, id = attempt.id);
            let outcome = async {
                info!(applied = ?prepared.applied.iter().map(|a| a.source.as_str()).collect::<Vec<_>>(), "starting attempt");
                let mut state = base_state.clone();
                state.attempt = attempt.n;
                state.started = Instant::now();
                state.notes = prepared.warnings.iter().chain(prepared.notes.iter()).cloned().collect();
                let first_delay = Duration::from_secs_f64(app.cfg.render.first_flush_delay_secs);
                let view = ReplyView::start(app.clone(), bot.clone(), chat_id, reply_to, turn_id, attempt.id, first_delay);
                let mut sink = Sink {
                    acc: Accumulator::new(),
                    view: &view,
                    state: state.clone(),
                    last_save: Instant::now(),
                    last_push: Instant::now() - Duration::from_secs(1),
                    app: &app,
                    attempt_id: attempt.id,
                };
                view.update(sink.render());
                let cancel = CancellationToken::new();
                let shutdown = app.shutdown.clone();
                let (result, call) = tokio::select! {
                    r = app.openrouter.stream_chat(&prepared, &mut sink, &rawlog, &cancel) => r,
                    _ = shutdown.cancelled() => (Err(LlmError::cancelled()), Default::default()),
                };
                if matches!(&result, Err(e) if e.kind == omni_llm::ErrorKind::Cancelled) && app.shutdown.is_cancelled() {
                    // Shutting down: leave the attempt "running" so it is resumed after restart.
                    return Outcome::Shutdown;
                }
                let mut acc = sink.acc;
                if !call.upstream_echo.is_empty() {
                    let echo = omni_core::json::redact_blobs(&Value::Array(call.upstream_echo.clone()), &mut |b, m| {
                        app.blobs.put_sync(b, m).unwrap_or_else(|_| omni_core::BlobRef::for_bytes(b, m))
                    });
                    tracing::debug!(target: "raw", echo = %echo, "upstream body echoed by OpenRouter");
                    let _ = save_echo(&app, attempt.id, echo).await;
                }
                // Store generated images.
                for (data, mime) in acc.take_pending_images() {
                    match app.blobs.put(data, mime).await {
                        Ok(b) => acc.attach_image(b),
                        Err(e) => warn!(error = %e, "failed to store generated image"),
                    }
                }
                let usage = acc.usage.clone();
                let upstream_id = acc.upstream_id.clone().or(call.upstream_id.clone());
                let mut final_state = sink_state(&state, &acc);
                final_state.show_usage = show_usage(&app, usage.as_ref());
                match result {
                    Ok(native) => {
                        acc.set_native(prepared.dialect, &prepared.model, native);
                        let turn = acc.turn();
                        conv::update_turn(&app.db, turn_id, &turn, TurnStatus::Done).await.ok();
                        conv::update_attempt(&app.db, attempt.id, AttemptUpdate {
                            status: "done",
                            output: serde_json::to_value(&turn).ok(),
                            error: None,
                            upstream_id: upstream_id.clone(),
                            usage: usage.as_ref().and_then(|u| serde_json::to_value(u).ok()),
                            finished: true,
                        })
                        .await
                        .ok();
                        record_usage(&app, &bot.name, chat_id, user_id, turn_id, attempt.id, &spec.provider, &spec.model, usage.as_ref(), upstream_id.as_deref()).await;
                        final_state.status = Status::Done;
                        final_state.done = true;
                        let ids = view.finish(final_state).await;
                        send_images(&app, &bot, chat_id, ids.first().copied().or(reply_to), turn_id, attempt.id, &turn).await;
                        info!(cost = ?usage.as_ref().and_then(|u| u.cost_usd), "attempt succeeded");
                        Outcome::Done
                    }
                    Err(e) => {
                        warn!(error = %e, retry = ?e.retry, "attempt failed");
                        let turn = acc.turn();
                        conv::update_attempt(&app.db, attempt.id, AttemptUpdate {
                            status: "failed",
                            output: serde_json::to_value(&turn).ok(),
                            error: serde_json::to_value(&e).ok(),
                            upstream_id: upstream_id.clone(),
                            usage: usage.as_ref().and_then(|u| serde_json::to_value(u).ok()),
                            finished: true,
                        })
                        .await
                        .ok();
                        record_usage(&app, &bot.name, chat_id, user_id, turn_id, attempt.id, &spec.provider, &spec.model, usage.as_ref(), upstream_id.as_deref()).await;
                        let failed_now = failed + 1;
                        let delay = backoff(&app, failed_now, &e);
                        let will_retry = delay.is_some() && failed_now < max_attempts;
                        final_state.error = Some(e.clone());
                        final_state.status = match (will_retry, delay) {
                            (true, Some(d)) => Status::Retrying { in_secs: d.as_secs() },
                            _ => Status::Failed,
                        };
                        final_state.done = true;
                        view.finish(final_state).await;
                        if will_retry { Outcome::Retry(delay.unwrap()) } else { Outcome::Failed }
                    }
                }
            }
            .instrument(attempt_span)
            .await;
            match outcome {
                Outcome::Done | Outcome::Shutdown => return Ok(()),
                Outcome::Failed => {
                    conv::set_turn_status(&app.db, turn_id, TurnStatus::Failed).await?;
                    return Ok(());
                }
                Outcome::Retry(d) => {
                    failed += 1;
                    tokio::select! {
                        _ = tokio::time::sleep(d) => {}
                        _ = app.shutdown.cancelled() => return Ok(()),
                    }
                }
            }
        }
    }
    .instrument(span)
    .await
}

enum Outcome {
    Done,
    Failed,
    Retry(Duration),
    Shutdown,
}

fn backoff(app: &App, failed: u32, e: &LlmError) -> Option<Duration> {
    let base = app.cfg.retry.base_delay_secs * 2f64.powi(failed.saturating_sub(1) as i32);
    let base = Duration::from_secs_f64(base.min(app.cfg.retry.max_delay_secs));
    e.retry_delay(base)
}

fn sink_state(base: &RenderState, acc: &Accumulator) -> RenderState {
    let mut s = base.clone();
    s.parts = acc.parts().to_vec();
    s.usage = acc.usage.clone();
    s.provider = acc.provider.clone();
    s.finish_reason = acc.finish_reason.clone();
    s.native_finish_reason = acc.native_finish_reason.clone();
    s.images = s.parts.iter().filter(|p| matches!(p, Part::Image { generated: true, .. })).count();
    for n in &acc.notices {
        if !s.notes.contains(n) {
            s.notes.push(n.clone());
        }
    }
    s
}

struct Sink<'a> {
    acc: Accumulator,
    view: &'a ReplyView,
    state: RenderState,
    last_save: Instant,
    /// Snapshots are pushed to the view at most every 250 ms (the view itself
    /// only edits Telegram every few seconds and always uses the newest one).
    last_push: Instant,
    app: &'a App,
    attempt_id: i64,
}

impl Sink<'_> {
    fn render(&self) -> RenderState {
        sink_state(&self.state, &self.acc)
    }
}

impl EventSink for Sink<'_> {
    fn event(&mut self, ev: StreamEvent) {
        let is_image = matches!(ev, StreamEvent::Image { .. });
        self.acc.apply(ev);
        if is_image {
            // Persist images immediately (they are large and should survive a crash).
            for (data, mime) in self.acc.take_pending_images() {
                match self.app.blobs.put_sync(&data, &mime) {
                    Ok(b) => self.acc.attach_image(b),
                    Err(e) => warn!(error = %e, "failed to store generated image"),
                }
            }
        }
        if self.last_push.elapsed() >= Duration::from_millis(250) {
            self.last_push = Instant::now();
            self.view.update(self.render());
        }
        if self.last_save.elapsed() > Duration::from_secs(10) {
            self.last_save = Instant::now();
            let (db, id, out) = (self.app.db.clone(), self.attempt_id, serde_json::to_value(self.acc.turn()).ok());
            tokio::spawn(async move {
                let _ = conv::update_attempt(
                    &db,
                    id,
                    AttemptUpdate {
                        status: "running",
                        output: out,
                        error: None,
                        upstream_id: None,
                        usage: None,
                        finished: false,
                    },
                )
                .await;
            });
        }
    }
}

async fn save_echo(app: &App, attempt_id: i64, echo: Value) -> Result<()> {
    let s = echo.to_string();
    app.db
        .call(move |c| {
            c.execute(
                "UPDATE attempts SET request = json_set(COALESCE(request, '{}'), '$.upstream_echo', json(?1)) WHERE id = ?2",
                params![s, attempt_id],
            )?;
            Ok(())
        })
        .await
}

/// A turn that fails before any upstream call (bad options, quota, failed parent...).
async fn fail_without_attempt(
    app: &Arc<App>,
    bot: &Arc<crate::tg::bot::BotConn>,
    chat_id: i64,
    reply_to: Option<i32>,
    turn_id: i64,
    mut st: RenderState,
    err: LlmError,
) -> Result<()> {
    warn!(error = %err, "turn failed before calling the model");
    let attempt = conv::start_attempt(&app.db, turn_id, &json!({"not_sent": true})).await?;
    conv::update_attempt(
        &app.db,
        attempt.id,
        AttemptUpdate {
            status: "failed",
            output: None,
            error: serde_json::to_value(&err).ok(),
            upstream_id: None,
            usage: None,
            finished: true,
        },
    )
    .await?;
    st.attempt = attempt.n;
    st.error = Some(err);
    st.status = Status::Failed;
    st.elapsed_override = Some(Duration::ZERO);
    let view = ReplyView::start(app.clone(), bot.clone(), chat_id, reply_to, turn_id, attempt.id, Duration::ZERO);
    view.finish(st).await;
    conv::set_turn_status(&app.db, turn_id, TurnStatus::Failed).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn record_usage(
    app: &Arc<App>,
    bot: &str,
    chat_id: i64,
    user_id: i64,
    turn_id: i64,
    attempt_id: i64,
    provider: &str,
    model: &str,
    usage: Option<&Usage>,
    upstream_id: Option<&str>,
) {
    let u = usage.cloned().unwrap_or_default();
    let (bot, provider, model) = (bot.to_string(), provider.to_string(), model.to_string());
    let raw = if u.raw.is_null() { None } else { Some(u.raw.to_string()) };
    let cost_source = match u.cost_usd {
        Some(_) => "reported",
        None => "unknown",
    };
    let has_cost = u.cost_usd.is_some();
    let res = app
        .db
        .call(move |c| {
            c.execute(
                "INSERT OR REPLACE INTO usage_ledger(ts, bot, chat_id, user_id, turn_id, attempt_id, provider, model,
                    input_tokens, output_tokens, reasoning_tokens, cached_tokens, cache_write_tokens, cost_usd, cost_source, raw)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    now_ms(), bot, chat_id, user_id, turn_id, attempt_id, provider, model,
                    u.input_tokens.map(|x| x as i64), u.output_tokens.map(|x| x as i64), u.reasoning_tokens.map(|x| x as i64),
                    u.cached_input_tokens.map(|x| x as i64), u.cache_write_tokens.map(|x| x as i64), u.cost_usd, cost_source, raw
                ],
            )?;
            Ok(())
        })
        .await;
    if let Err(e) = res {
        warn!(error = %e, "failed to record usage");
    }
    if let Some(c) = usage.and_then(|u| u.cost_usd) {
        let _ = crate::access::charge(&app.db, "user", user_id, c).await;
        if chat_id < 0 {
            let _ = crate::access::charge(&app.db, "chat", chat_id, c).await;
        }
    }
    // Cost unknown (e.g. stream dropped): ask OpenRouter afterwards.
    if !has_cost && let Some(gen_id) = upstream_id.map(str::to_string) {
        let app = app.clone();
        tokio::spawn(async move {
            for delay in [5u64, 30, 120] {
                tokio::time::sleep(Duration::from_secs(delay)).await;
                match app.openrouter.generation(&gen_id).await {
                    Ok(Some(d)) => {
                        let cost = d.get("total_cost").and_then(Value::as_f64);
                        let raw = d.to_string();
                        let _ = app
                                .db
                                .call(move |c| {
                                    c.execute(
                                        "UPDATE usage_ledger SET cost_usd = ?1, cost_source = 'fetched', raw = ?2 WHERE attempt_id = ?3",
                                        params![cost, raw, attempt_id],
                                    )?;
                                    Ok(())
                                })
                                .await;
                        if let Some(c) = cost {
                            let _ = crate::access::charge(&app.db, "user", user_id, c).await;
                            if chat_id < 0 {
                                let _ = crate::access::charge(&app.db, "chat", chat_id, c).await;
                            }
                        }
                        info!(generation = %gen_id, ?cost, "fetched cost of interrupted generation");
                        return;
                    }
                    Ok(None) => continue,
                    Err(e) => warn!(error = %e, "generation lookup failed"),
                }
            }
        });
    }
}

async fn send_images(
    app: &Arc<App>,
    bot: &Arc<crate::tg::bot::BotConn>,
    chat_id: i64,
    reply_to: Option<i32>,
    turn_id: i64,
    attempt_id: i64,
    turn: &Turn,
) {
    let mut idx = 0i64;
    for p in &turn.parts {
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
