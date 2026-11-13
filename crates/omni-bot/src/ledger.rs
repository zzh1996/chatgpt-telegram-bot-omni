//! Usage & cost ledger: one row per billed upstream call, idempotent by the
//! upstream generation id, so a call is never counted twice and never lost —
//! across stream drops, long-decode hops, restarts and crashes.
//!
//! - When a call's generation id becomes known (response headers), it is
//!   registered in `pending_costs`.
//! - When its usage arrives in the stream, a ledger row is inserted and the
//!   pending entry removed (one transaction).
//! - Calls whose usage never arrived (stream dropped, bot stopped or crashed)
//!   are looked up later via OpenRouter's `/generation` endpoint by
//!   [`spawn_settler`].
//!
//! Spend limits are charged exactly when a row is inserted.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use omni_core::Usage;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::app::App;
use crate::store::{Db, now_ms};

/// Who/what a call is attributed to.
#[derive(Debug, Clone)]
pub struct CallMeta {
    pub bot: String,
    pub chat_id: i64,
    pub user_id: i64,
    pub turn_id: i64,
    pub attempt_id: i64,
    pub provider: String,
    pub model: String,
}

/// First lookup delay: `/generation` becomes available a few seconds after a call ends.
const FIRST_LOOKUP_MS: i64 = 15_000;
/// Give up looking a call up after this many tries (~2 days with the backoff below).
const MAX_TRIES: i64 = 12;

/// Register a call whose usage is not known yet.
pub async fn begin_call(db: &Db, generation_id: String, attempt_id: i64) {
    let res = db
        .call(move |c| {
            c.execute(
                "INSERT OR IGNORE INTO pending_costs(generation_id, attempt_id, created_at, next_at) VALUES (?1, ?2, ?3, ?4)",
                params![generation_id, attempt_id, now_ms(), now_ms() + FIRST_LOOKUP_MS],
            )?;
            Ok(())
        })
        .await;
    if let Err(e) = res {
        warn!(error = %format!("{e:#}"), "failed to register pending call");
    }
}

/// Record a call's usage (once per generation id). Charges spend limits when
/// the row is new. Returns whether it was new.
/// The ledger's `cost_source` for a usage: estimated from a price table, or reported by the API.
pub fn source_of(u: &Usage) -> &'static str {
    match u.cost_source {
        omni_core::usage::CostSource::Reported => "reported",
        omni_core::usage::CostSource::Fetched => "fetched",
        omni_core::usage::CostSource::Estimated => "estimated",
        omni_core::usage::CostSource::Unknown => "unknown",
    }
}

pub async fn record_call(
    db: &Db,
    meta: &CallMeta,
    generation_id: Option<String>,
    usage: &Usage,
    source: &'static str,
) -> Result<bool> {
    let m = meta.clone();
    let u = usage.clone();
    let raw = if u.raw.is_null() { None } else { Some(u.raw.to_string()) };
    let source = if u.cost_usd.is_some() { source } else { "unknown" };
    let gid = generation_id.clone();
    let inserted = db
        .call(move |c| {
            let tx = c.transaction()?;
            let n = tx.execute(
                "INSERT OR IGNORE INTO usage_ledger(ts, bot, chat_id, user_id, turn_id, attempt_id, generation_id, provider,
                    model, input_tokens, output_tokens, reasoning_tokens, cached_tokens, cache_write_tokens, cost_usd,
                    cost_source, raw)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                params![
                    now_ms(),
                    m.bot,
                    m.chat_id,
                    m.user_id,
                    m.turn_id,
                    m.attempt_id,
                    gid,
                    m.provider,
                    m.model,
                    u.input_tokens.map(|x| x as i64),
                    u.output_tokens.map(|x| x as i64),
                    u.reasoning_tokens.map(|x| x as i64),
                    u.cached_input_tokens.map(|x| x as i64),
                    u.cache_write_tokens.map(|x| x as i64),
                    u.cost_usd,
                    source,
                    raw
                ],
            )?;
            if let Some(g) = &gid {
                tx.execute("DELETE FROM pending_costs WHERE generation_id = ?1", params![g])?;
            }
            tx.commit()?;
            Ok(n > 0)
        })
        .await?;
    if inserted && let Some(c) = usage.cost_usd {
        crate::access::charge(db, &meta.bot, meta.chat_id, meta.user_id, c).await?;
    }
    Ok(inserted)
}

/// Usage from an OpenRouter `/generation` record.
pub fn usage_from_generation(d: &Value) -> Usage {
    let n = |k: &str| d.get(k).and_then(Value::as_u64);
    Usage {
        input_tokens: n("native_tokens_prompt").or(n("tokens_prompt")),
        output_tokens: n("native_tokens_completion").or(n("tokens_completion")),
        reasoning_tokens: n("native_tokens_reasoning"),
        cached_input_tokens: n("native_tokens_cached"),
        cache_write_tokens: None,
        // `usage` is what was charged; `total_cost` is the older name.
        cost_usd: d.get("usage").and_then(Value::as_f64).or_else(|| d.get("total_cost").and_then(Value::as_f64)),
        cost_source: Default::default(),
        raw: d.clone(),
    }
}

async fn meta_for_attempt(db: &Db, attempt_id: i64) -> Result<Option<CallMeta>> {
    db.call(move |c| {
        Ok(c.query_row(
            "SELECT t.bot, t.chat_id, t.user_id, t.id, json_extract(t.spec, '$.provider'), json_extract(t.spec, '$.model')
             FROM attempts a JOIN turns t ON t.id = a.turn_id WHERE a.id = ?1",
            params![attempt_id],
            |r| {
                Ok(CallMeta {
                    bot: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    chat_id: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    user_id: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    turn_id: r.get(3)?,
                    attempt_id,
                    provider: r.get::<_, Option<String>>(4)?.unwrap_or_else(|| "openrouter".into()),
                    model: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                })
            },
        )
        .optional()?)
    })
    .await
}

/// Look up pending calls in the background, forever.
pub fn spawn_settler(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = settle_once(&app).await {
                warn!(error = %format!("{e:#}"), "settling pending costs failed");
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(20)) => {}
                _ = app.shutdown.cancelled() => return,
            }
        }
    });
}

async fn settle_once(app: &App) -> Result<()> {
    let due: Vec<(String, i64, i64)> = app
        .db
        .call(|c| {
            let mut s = c.prepare(
                "SELECT generation_id, attempt_id, tries FROM pending_costs WHERE next_at <= ?1 ORDER BY next_at LIMIT 50",
            )?;
            let v = s.query_map(params![now_ms()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
            Ok(v)
        })
        .await?;
    for (gid, attempt_id, tries) in due {
        let already = {
            let g = gid.clone();
            app.db
                .call(move |c| {
                    let n: i64 =
                        c.query_row("SELECT COUNT(*) FROM usage_ledger WHERE generation_id = ?1", params![g], |r| {
                            r.get(0)
                        })?;
                    if n > 0 {
                        c.execute("DELETE FROM pending_costs WHERE generation_id = ?1", params![g])?;
                    }
                    Ok(n > 0)
                })
                .await?
        };
        if already {
            continue;
        }
        let found = match app.openrouter.generation(&gid).await {
            Ok(v) => v,
            Err(e) => {
                debug!(generation = %gid, error = %format!("{e:#}"), "generation lookup failed");
                None
            }
        };
        match found {
            Some(d) => {
                let Some(meta) = meta_for_attempt(&app.db, attempt_id).await? else {
                    forget(&app.db, &gid).await?;
                    continue;
                };
                let usage = usage_from_generation(&d);
                record_call(&app.db, &meta, Some(gid.clone()), &usage, "fetched").await?;
                info!(generation = %gid, cost = ?usage.cost_usd, "recorded usage of a call whose stream did not finish");
            }
            None if tries + 1 >= MAX_TRIES => {
                warn!(generation = %gid, "giving up looking up the cost of an unfinished call");
                forget(&app.db, &gid).await?;
            }
            None => {
                let g = gid.clone();
                // 15s, 30s, 1m, 2m, ... capped at 6h
                let delay = (FIRST_LOOKUP_MS << (tries + 1).min(10)).min(6 * 3_600_000);
                app.db
                    .call(move |c| {
                        c.execute(
                            "UPDATE pending_costs SET tries = tries + 1, next_at = ?1 WHERE generation_id = ?2",
                            params![now_ms() + delay, g],
                        )?;
                        Ok(())
                    })
                    .await?;
            }
        }
    }
    Ok(())
}

async fn forget(db: &Db, gid: &str) -> Result<()> {
    let g = gid.to_string();
    db.call(move |c| {
        c.execute("DELETE FROM pending_costs WHERE generation_id = ?1", params![g])?;
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta() -> CallMeta {
        CallMeta {
            bot: "b".into(),
            chat_id: -100,
            user_id: 7,
            turn_id: 1,
            attempt_id: 1,
            provider: "openrouter".into(),
            model: "m/x".into(),
        }
    }

    #[tokio::test]
    async fn calls_are_counted_once() {
        let db = Db::open_in_memory().unwrap();
        begin_call(&db, "gen-1".into(), 1).await;
        let u = Usage { input_tokens: Some(10), output_tokens: Some(5), cost_usd: Some(0.01), ..Default::default() };
        assert!(record_call(&db, &meta(), Some("gen-1".into()), &u, "reported").await.unwrap());
        assert!(!record_call(&db, &meta(), Some("gen-1".into()), &u, "fetched").await.unwrap());
        let (rows, pending): (i64, i64) = db
            .call(|c| {
                Ok((
                    c.query_row("SELECT COUNT(*) FROM usage_ledger", [], |r| r.get(0))?,
                    c.query_row("SELECT COUNT(*) FROM pending_costs", [], |r| r.get(0))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!((rows, pending), (1, 0));
    }

    #[test]
    fn generation_usage_fields() {
        let u = usage_from_generation(
            &json!({"usage": 0.25, "total_cost": 0.3, "native_tokens_prompt": 12, "native_tokens_completion": 34}),
        );
        assert_eq!(u.cost_usd, Some(0.25));
        assert_eq!((u.input_tokens, u.output_tokens), (Some(12), Some(34)));
    }
}
