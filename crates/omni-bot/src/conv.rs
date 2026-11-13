//! Conversation persistence: inbound queue, turns, attempts, Telegram message mapping.

use anyhow::{Context, Result, bail};
use omni_core::trigger::OptionToken;
use omni_core::{Role, Turn};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::{Db, now_ms};

/// The resolved request specification of an assistant turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedSpec {
    /// Provider id, e.g. `openrouter`.
    pub provider: String,
    /// Model id as known by the provider.
    pub model: String,
    /// Short display label (alias or model id).
    pub label: String,
    /// Options: alias options followed by user options.
    #[serde(default)]
    pub options: Vec<OptionToken>,
    /// The spec as typed by the user.
    #[serde(default)]
    pub raw: String,
    /// A chat prefix's system prompt and history, put before the new message
    /// when a conversation starts (never stored with the turn: the turns are).
    #[serde(skip)]
    pub template: Option<std::sync::Arc<Vec<Turn>>>,
}

impl ResolvedSpec {
    pub fn canonical(&self) -> String {
        format!("{}:{}", self.provider, self.model)
    }

    /// The model as shown in replies: OpenRouter ids as they are
    /// (`openai/gpt-6.1-sol`), the platforms' own APIs with their prefix
    /// (`openai:gpt-6.1-sol`), so the route of an answer is always visible.
    pub fn display_model(&self) -> String {
        if self.provider == "openrouter" { self.model.clone() } else { self.canonical() }
    }

    pub fn options_syntax(&self) -> String {
        self.options.iter().map(|o| format!("+{}", o.to_syntax())).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Pending,
    Running,
    Done,
    Failed,
}

impl TurnStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnStatus::Pending => "pending",
            TurnStatus::Running => "running",
            TurnStatus::Done => "done",
            TurnStatus::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> TurnStatus {
        match s {
            "pending" => TurnStatus::Pending,
            "running" => TurnStatus::Running,
            "failed" | "cancelled" => TurnStatus::Failed,
            _ => TurnStatus::Done,
        }
    }
    pub fn is_finished(self) -> bool {
        matches!(self, TurnStatus::Done | TurnStatus::Failed)
    }
}

#[derive(Debug, Clone)]
pub struct TurnRow {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub turn: Turn,
    pub spec: Option<ResolvedSpec>,
    pub status: TurnStatus,
    pub bot: Option<String>,
    pub chat_id: Option<i64>,
    pub user_id: Option<i64>,
    pub inbound_id: Option<i64>,
    #[allow(dead_code)]
    pub created_at: i64,
}

fn row_to_turn(
    r: &rusqlite::Row<'_>,
) -> rusqlite::Result<(
    i64,
    Option<i64>,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    i64,
)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?))
}

const TURN_COLS: &str = "id, parent_id, data, spec, status, bot, chat_id, user_id, inbound_id, created_at";

fn decode_turn(
    t: (i64, Option<i64>, String, Option<String>, String, Option<String>, Option<i64>, Option<i64>, Option<i64>, i64),
) -> Result<TurnRow> {
    let (id, parent_id, data, spec, status, bot, chat_id, user_id, inbound_id, created_at) = t;
    Ok(TurnRow {
        id,
        parent_id,
        turn: serde_json::from_str(&data).with_context(|| format!("decode turn {id}"))?,
        spec: spec
            .map(|s| serde_json::from_str(&s))
            .transpose()
            .with_context(|| format!("decode spec of turn {id}"))?,
        status: TurnStatus::parse(&status),
        bot,
        chat_id,
        user_id,
        inbound_id,
        created_at,
    })
}

pub fn get_turn_sync(c: &Connection, id: i64) -> Result<TurnRow> {
    let t = c.query_row(&format!("SELECT {TURN_COLS} FROM turns WHERE id = ?1"), params![id], row_to_turn)?;
    decode_turn(t)
}

pub async fn get_turn(db: &Db, id: i64) -> Result<TurnRow> {
    db.call(move |c| get_turn_sync(c, id)).await
}

/// Root..=turn chain.
pub async fn history(db: &Db, id: i64) -> Result<Vec<TurnRow>> {
    db.call(move |c| {
        let mut out = Vec::new();
        let mut cur = Some(id);
        while let Some(i) = cur {
            let t = get_turn_sync(c, i)?;
            cur = t.parent_id;
            out.push(t);
            if out.len() > 100_000 {
                bail!("conversation chain too long or cyclic");
            }
        }
        out.reverse();
        Ok(out)
    })
    .await
}

pub struct NewTurn {
    pub parent_id: Option<i64>,
    pub turn: Turn,
    pub spec: Option<ResolvedSpec>,
    pub status: TurnStatus,
    pub bot: Option<String>,
    pub chat_id: Option<i64>,
    pub user_id: Option<i64>,
    pub inbound_id: Option<i64>,
}

pub fn insert_turn_sync(c: &Connection, t: &NewTurn) -> Result<i64> {
    let now = now_ms();
    c.execute(
        "INSERT INTO turns(parent_id, role, data, spec, status, bot, chat_id, user_id, inbound_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
        params![
            t.parent_id,
            t.turn.role.as_str(),
            serde_json::to_string(&t.turn)?,
            t.spec.as_ref().map(serde_json::to_string).transpose()?,
            t.status.as_str(),
            t.bot,
            t.chat_id,
            t.user_id,
            t.inbound_id,
            now
        ],
    )?;
    Ok(c.last_insert_rowid())
}

pub async fn update_turn(db: &Db, id: i64, turn: &Turn, status: TurnStatus) -> Result<()> {
    let data = serde_json::to_string(turn)?;
    db.call(move |c| {
        c.execute(
            "UPDATE turns SET data = ?1, status = ?2, updated_at = ?3 WHERE id = ?4",
            params![data, status.as_str(), now_ms(), id],
        )?;
        Ok(())
    })
    .await
}

pub async fn set_turn_status(db: &Db, id: i64, status: TurnStatus) -> Result<()> {
    db.call(move |c| {
        c.execute(
            "UPDATE turns SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status.as_str(), now_ms(), id],
        )?;
        Ok(())
    })
    .await
}

/// Claim a failed turn for a manual retry: atomically flip it from failed to
/// pending, and mark its failed attempts superseded (the automatic retry count
/// starts afresh). `false` if it is not failed (any more): when several people
/// send /retry or press Retry at once, exactly one of them gets the retry.
pub async fn claim_retry(db: &Db, turn_id: i64) -> Result<bool> {
    db.call(move |c| {
        let tx = c.transaction()?;
        let n = tx.execute(
            "UPDATE turns SET status = 'pending', updated_at = ?1 WHERE id = ?2 AND status IN ('failed', 'cancelled')",
            params![now_ms(), turn_id],
        )?;
        if n == 1 {
            tx.execute(
                "UPDATE attempts SET status = 'superseded' WHERE turn_id = ?1 AND status = 'failed'",
                params![turn_id],
            )?;
        }
        tx.commit()?;
        Ok(n == 1)
    })
    .await
}

/// Assistant turns that still need (or were in the middle of) generation.
pub async fn unfinished_turns(db: &Db) -> Result<Vec<i64>> {
    db.call(|c| {
        let mut s = c.prepare(
            "SELECT id FROM turns WHERE role = 'assistant' AND status IN ('pending', 'running') ORDER BY id",
        )?;
        let v = s.query_map([], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?;
        Ok(v)
    })
    .await
}

/// Which turn (and attempt) a Telegram message belongs to.
pub async fn turn_for_message(db: &Db, bot: &str, chat_id: i64, msg_id: i32) -> Result<Option<(i64, Option<i64>)>> {
    let bot = bot.to_string();
    db.call(move |c| {
        Ok(c.query_row(
            "SELECT turn_id, attempt_id FROM tg_messages WHERE bot = ?1 AND chat_id = ?2 AND msg_id = ?3",
            params![bot, chat_id, msg_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
    })
    .await
}

/// Whether `bot` sent this message (an answer, command output or notice).
pub async fn sent_by(db: &Db, bot: &str, chat_id: i64, msg_id: i32) -> Result<bool> {
    let bot = bot.to_string();
    db.call(move |c| {
        Ok(c.query_row(
            "SELECT 1 FROM outbox WHERE bot = ?1 AND chat_id = ?2 AND msg_id = ?3 LIMIT 1",
            params![bot, chat_id, msg_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
    })
    .await
}

/// Map a Telegram message to a turn (tests; production code links via the outbox).
#[cfg(test)]
pub async fn link_message(
    db: &Db,
    bot: &str,
    chat_id: i64,
    msg_id: i32,
    turn_id: i64,
    attempt_id: Option<i64>,
    part: i64,
) -> Result<()> {
    let bot = bot.to_string();
    db.call(move |c| {
        c.execute(
            "INSERT OR REPLACE INTO tg_messages(bot, chat_id, msg_id, turn_id, attempt_id, part_index) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![bot, chat_id, msg_id, turn_id, attempt_id, part],
        )?;
        Ok(())
    })
    .await
}

#[derive(Debug, Clone)]
pub struct AttemptRow {
    pub id: i64,
    pub n: i64,
    pub status: String,
}

pub async fn start_attempt(db: &Db, turn_id: i64, request: &Value) -> Result<AttemptRow> {
    let req = serde_json::to_string(request)?;
    db.call(move |c| {
        let tx = c.transaction()?;
        let n: i64 =
            tx.query_row("SELECT COALESCE(MAX(n), 0) + 1 FROM attempts WHERE turn_id = ?1", params![turn_id], |r| {
                r.get(0)
            })?;
        tx.execute(
            "INSERT INTO attempts(turn_id, n, status, request, started_at) VALUES (?1, ?2, 'running', ?3, ?4)",
            params![turn_id, n, req, now_ms()],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute("UPDATE turns SET status = 'running', updated_at = ?1 WHERE id = ?2", params![now_ms(), turn_id])?;
        tx.commit()?;
        Ok(AttemptRow { id, n, status: "running".into() })
    })
    .await
}

pub struct AttemptUpdate {
    pub status: &'static str,
    pub output: Option<Value>,
    pub error: Option<Value>,
    pub upstream_id: Option<String>,
    pub usage: Option<Value>,
    pub finished: bool,
}

pub async fn update_attempt(db: &Db, id: i64, u: AttemptUpdate) -> Result<()> {
    let output = u.output.map(|v| v.to_string());
    let error = u.error.map(|v| v.to_string());
    let usage = u.usage.map(|v| v.to_string());
    db.call(move |c| {
        c.execute(
            "UPDATE attempts SET status = ?1, output = COALESCE(?2, output), error = COALESCE(?3, error),
                upstream_id = COALESCE(?4, upstream_id), usage = COALESCE(?5, usage),
                finished_at = CASE WHEN ?6 THEN ?7 ELSE finished_at END
             WHERE id = ?8",
            params![u.status, output, error, u.upstream_id, usage, u.finished, now_ms(), id],
        )?;
        Ok(())
    })
    .await
}

/// Store what the upstream calls of an attempt reported about themselves (`/info`).
pub async fn set_attempt_meta(db: &Db, id: i64, meta: &Value) -> Result<()> {
    let m = meta.to_string();
    db.call(move |c| {
        c.execute("UPDATE attempts SET meta = ?1 WHERE id = ?2", params![m, id])?;
        Ok(())
    })
    .await
}

pub async fn attempt_meta(db: &Db, id: i64) -> Result<Option<Value>> {
    let s: Option<String> = db
        .call(move |c| {
            Ok(c.query_row("SELECT meta FROM attempts WHERE id = ?1", params![id], |r| r.get(0)).optional()?.flatten())
        })
        .await?;
    Ok(s.and_then(|s| serde_json::from_str(&s).ok()))
}

/// Attempts of a turn: (id, n, status).
pub async fn attempts_of(db: &Db, turn_id: i64) -> Result<Vec<AttemptRow>> {
    db.call(move |c| {
        let mut s = c.prepare("SELECT id, n, status FROM attempts WHERE turn_id = ?1 ORDER BY n")?;
        let v = s
            .query_map(params![turn_id], |r| Ok(AttemptRow { id: r.get(0)?, n: r.get(1)?, status: r.get(2)? }))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    })
    .await
}

/// Whether an attempt continues a finished answer (`/continue`): it has its own messages.
fn is_continuation_sql() -> &'static str {
    "COALESCE(json_extract(a.request, '$.continue'), 0) = 1"
}

/// The Telegram messages showing a reply, in order: (msg_id, content_hash).
///
/// All attempts of a reply share its messages (each new attempt re-renders
/// them with the earlier attempts folded above); a `/continue` attempt has its
/// own messages after the answer.
pub async fn reply_messages(
    db: &Db,
    bot: &str,
    turn_id: i64,
    attempt_id: i64,
) -> Result<Vec<(i32, Option<String>, Option<crate::tg::flood::Shape>)>> {
    let bot = bot.to_string();
    db.call(move |c| {
        let cont: bool = c.query_row(
            &format!("SELECT {} FROM attempts a WHERE a.id = ?1", is_continuation_sql()),
            params![attempt_id],
            |r| r.get(0),
        )?;
        let (filter, key) = if cont {
            ("o.attempt_id = ?2".to_string(), attempt_id)
        } else {
            (format!("o.turn_id = ?2 AND NOT {}", is_continuation_sql()), turn_id)
        };
        let mut s = c.prepare(&format!(
            "SELECT o.msg_id, o.content_hash, o.shape_chars, o.shape_top FROM outbox o JOIN attempts a ON a.id = o.attempt_id
             WHERE o.bot = ?1 AND {filter} AND o.state = 'sent' AND o.part_index < 1000 ORDER BY o.msg_id"
        ))?;
        let v = s
            .query_map(params![bot, key], |r| {
                let (chars, top): (Option<i64>, Option<i64>) = (r.get(2)?, r.get(3)?);
                let shape = chars.zip(top).map(|(c, t)| crate::tg::flood::Shape { chars: c as usize, top_blocks: t as usize });
                Ok((r.get(0)?, r.get(1)?, shape))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    })
    .await
}

/// Earlier attempts of a reply that did not succeed (failed, interrupted), oldest
/// first, as shown above the current attempt. `/continue` attempts are separate.
pub async fn prior_attempts(db: &Db, turn_id: i64, before_n: i64) -> Result<Vec<crate::compose::PriorAttempt>> {
    let rows: Vec<(i64, String, Option<String>, Option<String>, Option<String>, i64, Option<i64>)> = db
        .call(move |c| {
            let mut s = c.prepare(&format!(
                "SELECT a.n, a.status, a.output, a.error, a.usage, a.started_at, a.finished_at FROM attempts a
                 WHERE a.turn_id = ?1 AND a.n < ?2 AND a.status IN ('failed', 'interrupted', 'superseded', 'cancelled')
                   AND NOT {} ORDER BY a.n",
                is_continuation_sql()
            ))?;
            let v = s
                .query_map(params![turn_id, before_n], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(n, status, out, err, usage, started, finished)| crate::compose::PriorAttempt {
            n,
            parts: out
                .and_then(|o| serde_json::from_str::<omni_core::Turn>(&o).ok())
                .map(|t| t.parts)
                .unwrap_or_default(),
            error: err.and_then(|e| serde_json::from_str::<omni_llm::LlmError>(&e).ok()),
            interrupted: status == "interrupted",
            usage: usage.and_then(|u| serde_json::from_str(&u).ok()),
            elapsed: finished.filter(|f| *f >= started).map(|f| std::time::Duration::from_millis((f - started) as u64)),
        })
        .collect())
}

pub async fn attempt_json(
    db: &Db,
    attempt_id: i64,
) -> Result<(Option<Value>, Option<Value>, Option<Value>, Option<String>, Option<Value>)> {
    db.call(move |c| {
        let (req, out, err, up, usage): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = c.query_row(
            "SELECT request, output, error, upstream_id, usage FROM attempts WHERE id = ?1",
            params![attempt_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let p = |s: Option<String>| s.and_then(|s| serde_json::from_str(&s).ok());
        Ok((p(req), p(out), p(err), up, p(usage)))
    })
    .await
}

// ------------------------------------------------------------------ inbound

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InboundPayload {
    /// Media attached to this message.
    #[serde(default)]
    pub media: Vec<crate::media::MediaRef>,
    /// Positions shared in this message (location, venue, live location, map blocks).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<crate::media::LocationRef>,
    /// The message is a rich message (its text is the Markdown of its blocks).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rich: bool,
    /// Polls, contacts, dice, to-do lists, as text (`media::cards_of`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cards: Vec<String>,
    #[serde(default)]
    pub sender_name: Option<String>,
    #[serde(default)]
    pub sender_username: Option<String>,
    /// The reply quotes part of the replied message (Telegram "quote").
    #[serde(default)]
    pub has_quote: bool,
    #[serde(default)]
    pub chat_title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Inbound {
    pub id: i64,
    pub bot: String,
    pub chat_id: i64,
    pub msg_id: i32,
    pub sender_id: Option<i64>,
    #[allow(dead_code)]
    pub date: i64,
    pub text: String,
    pub reply_to_msg_id: Option<i32>,
    pub grouped_id: Option<i64>,
    pub payload: InboundPayload,
    pub status: String,
}

pub async fn get_inbound(db: &Db, id: i64) -> Result<Inbound> {
    db.call(move |c| {
        let row = c.query_row(
            "SELECT id, bot, chat_id, msg_id, sender_id, date, text, reply_to_msg_id, grouped_id, payload, status FROM inbound WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i32>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Option<i32>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, String>(10)?,
                ))
            },
        )?;
        Ok(Inbound {
            id: row.0,
            bot: row.1,
            chat_id: row.2,
            msg_id: row.3,
            sender_id: row.4,
            date: row.5,
            text: row.6,
            reply_to_msg_id: row.7,
            grouped_id: row.8,
            payload: serde_json::from_str(&row.9).unwrap_or_default(),
            status: row.10,
        })
    })
    .await
}

/// The first recorded message of an album (the one that handles it).
pub async fn album_leader(db: &Db, bot: &str, chat_id: i64, grouped_id: i64) -> Result<Option<i32>> {
    let bot = bot.to_string();
    db.call(move |c| {
        Ok(c.query_row(
            "SELECT MIN(msg_id) FROM inbound WHERE bot = ?1 AND chat_id = ?2 AND grouped_id = ?3",
            params![bot, chat_id, grouped_id],
            |r| r.get::<_, Option<i32>>(0),
        )?)
    })
    .await
}

pub async fn set_inbound_status(db: &Db, id: i64, status: &str, error: Option<String>) -> Result<()> {
    let status = status.to_string();
    db.call(move |c| {
        c.execute("UPDATE inbound SET status = ?1, error = ?2 WHERE id = ?3", params![status, error, id])?;
        Ok(())
    })
    .await
}

pub async fn pending_inbound(db: &Db) -> Result<Vec<i64>> {
    db.call(|c| {
        let mut s = c.prepare("SELECT id FROM inbound WHERE status = 'new' ORDER BY id")?;
        let v = s.query_map([], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?;
        Ok(v)
    })
    .await
}

/// Turns created from an inbound message (idempotency of the handler).
pub async fn turns_of_inbound(db: &Db, inbound_id: i64) -> Result<Vec<(i64, Role)>> {
    db.call(move |c| {
        let mut s = c.prepare("SELECT id, role FROM turns WHERE inbound_id = ?1 ORDER BY id")?;
        let v = s
            .query_map(params![inbound_id], |r| {
                let role: String = r.get(1)?;
                Ok((
                    r.get(0)?,
                    match role.as_str() {
                        "system" => Role::System,
                        "user" => Role::User,
                        _ => Role::Assistant,
                    },
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn turn_chain_and_attempts() {
        let db = Db::open_in_memory().unwrap();
        let (a, b) = db
            .call(|c| {
                let a = insert_turn_sync(
                    c,
                    &NewTurn {
                        parent_id: None,
                        turn: Turn::user_text("hi"),
                        spec: None,
                        status: TurnStatus::Done,
                        bot: Some("b".into()),
                        chat_id: Some(1),
                        user_id: Some(2),
                        inbound_id: None,
                    },
                )?;
                let b = insert_turn_sync(
                    c,
                    &NewTurn {
                        parent_id: Some(a),
                        turn: Turn { role: Role::Assistant, origin: Default::default(), parts: vec![], native: None },
                        spec: Some(ResolvedSpec {
                            provider: "openrouter".into(),
                            model: "x/y".into(),
                            label: "y".into(),
                            options: vec![],
                            raw: "y".into(),
                            template: None,
                        }),
                        status: TurnStatus::Pending,
                        bot: Some("b".into()),
                        chat_id: Some(1),
                        user_id: Some(2),
                        inbound_id: None,
                    },
                )?;
                Ok((a, b))
            })
            .await
            .unwrap();
        let h = history(&db, b).await.unwrap();
        assert_eq!(h.iter().map(|t| t.id).collect::<Vec<_>>(), vec![a, b]);
        assert_eq!(unfinished_turns(&db).await.unwrap(), vec![b]);
        let at = start_attempt(&db, b, &serde_json::json!({"x": 1})).await.unwrap();
        assert_eq!(at.n, 1);
        let at2 = start_attempt(&db, b, &serde_json::json!({})).await.unwrap();
        assert_eq!(at2.n, 2);
        assert_eq!(get_turn(&db, b).await.unwrap().status, TurnStatus::Running);
        link_message(&db, "b", 1, 77, b, Some(at.id), 0).await.unwrap();
        assert_eq!(turn_for_message(&db, "b", 1, 77).await.unwrap(), Some((b, Some(at.id))));
        assert_eq!(turn_for_message(&db, "other", 1, 77).await.unwrap(), None);

        // Retries share the reply's messages; `/continue` has its own; earlier
        // failed attempts are listed in order with their output and error.
        let e = omni_llm::LlmError::request("boom");
        update_attempt(
            &db,
            at.id,
            AttemptUpdate {
                status: "failed",
                output: serde_json::to_value(Turn::assistant_text("partial")).ok(),
                error: serde_json::to_value(&e).ok(),
                upstream_id: None,
                usage: Some(serde_json::json!({"output_tokens": 3})),
                finished: true,
            },
        )
        .await
        .unwrap();
        let cont = start_attempt(&db, b, &serde_json::json!({"continue": true})).await.unwrap();
        let (a1, a2, a3) = (at.id, at2.id, cont.id);
        db.call(move |c| {
            let row = |attempt: i64, part: i64, msg: i32, state: &str| {
                c.execute(
                    "INSERT INTO outbox(bot, chat_id, turn_id, attempt_id, part_index, random_id, state, msg_id, content_hash, created_at, updated_at)
                     VALUES ('b', 1, ?1, ?2, ?3, ?4, ?5, ?6, 'h', 0, 0)",
                    params![b, attempt, part, msg as i64 * 7, state, msg],
                )
            };
            row(a1, 0, 100, "sent")?;
            row(a1, 1, 101, "deleted")?;
            row(a2, 1, 105, "sent")?;
            row(a3, 0, 200, "sent")?;
            Ok(())
        })
        .await
        .unwrap();
        let ids = |v: Vec<(i32, Option<String>, Option<crate::tg::flood::Shape>)>| {
            v.into_iter().map(|(id, _, _)| id).collect::<Vec<_>>()
        };
        assert_eq!(ids(reply_messages(&db, "b", b, a2).await.unwrap()), vec![100, 105]);
        assert_eq!(ids(reply_messages(&db, "b", b, a1).await.unwrap()), vec![100, 105]);
        assert_eq!(ids(reply_messages(&db, "b", b, a3).await.unwrap()), vec![200]);
        // Concurrent manual retries: only the first claim wins.
        set_turn_status(&db, b, TurnStatus::Failed).await.unwrap();
        assert!(claim_retry(&db, b).await.unwrap());
        assert!(!claim_retry(&db, b).await.unwrap());
        assert_eq!(get_turn(&db, b).await.unwrap().status, TurnStatus::Pending);
        let st: String = db
            .call(move |c| Ok(c.query_row("SELECT status FROM attempts WHERE id = ?1", params![a1], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(st, "superseded");
        let prior = prior_attempts(&db, b, 4).await.unwrap();
        assert_eq!(prior.len(), 1);
        assert_eq!(prior[0].n, 1);
        assert_eq!(prior[0].error.as_ref().map(|e| e.message.as_str()), Some("boom"));
        assert_eq!(prior[0].usage.as_ref().and_then(|u| u.output_tokens), Some(3));
        assert!(matches!(&prior[0].parts[..], [omni_core::Part::Text { text, .. }] if text == "partial"));
    }
}
