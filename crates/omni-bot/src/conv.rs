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
}

impl ResolvedSpec {
    pub fn canonical(&self) -> String {
        format!("{}:{}", self.provider, self.model)
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

/// Messages already sent for an attempt: (part_index, msg_id, content_hash).
pub async fn attempt_messages(db: &Db, bot: &str, attempt_id: i64) -> Result<Vec<(i64, i32, Option<String>)>> {
    let bot = bot.to_string();
    db.call(move |c| {
        let mut s = c.prepare(
            "SELECT part_index, msg_id, content_hash FROM outbox
             WHERE bot = ?1 AND attempt_id = ?2 AND state = 'sent' AND part_index < 1000 ORDER BY part_index",
        )?;
        let v = s
            .query_map(params![bot, attempt_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    })
    .await
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
    /// Media of the message this one replies to (when it is not a bot message).
    #[serde(default)]
    pub reply_media: Vec<crate::media::MediaRef>,
    #[serde(default)]
    pub reply_grouped_id: Option<i64>,
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
    }
}
